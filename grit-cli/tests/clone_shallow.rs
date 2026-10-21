//! `grit clone --shallow` matches `git clone --depth=1`.

use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn clone_shallow_flag_limits_history() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = base.path().join("remote.git");
    let wt = base.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git_in(&wt, &["init", "-q", "-b", "main", "."]);
    for i in 1..=3 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git_in(&wt, &["add", "f"]);
        git_in(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git_in(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);

    let dest = base.path().join("clone");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--shallow",
            bare.to_str().unwrap(),
            dest.to_str().unwrap(),
        ])
        .output()
        .expect("grit clone");
    assert!(
        out.status.success(),
        "grit clone --shallow failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(dest.join(".git/shallow").is_file());
    let log = git_out(&dest, &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1, "expected one commit: {log}");

    let fsck = Command::new("git")
        .args(["fsck"])
        .current_dir(&dest)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}
