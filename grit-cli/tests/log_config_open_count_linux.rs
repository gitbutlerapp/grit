//! Linux regression: `grit log --limit 1` must not re-open config and commondir repeatedly.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn count_path_opens(strace_log: &str, needle: &str) -> usize {
    strace_log
        .lines()
        .filter(|line| {
            (line.contains("openat(") || line.contains("open(")) && line.contains(needle)
        })
        .count()
}

#[test]
fn log_limit_one_opens_config_and_commondir_at_most_twice_and_once() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(root)
        .status()
        .expect("git init")
        .success());
    Command::new("git")
        .args(["config", "user.email", "t@example.com"])
        .current_dir(root)
        .status()
        .expect("email");
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(root)
        .status()
        .expect("name");
    std::fs::write(root.join("README"), b"hello\n").expect("readme");
    Command::new("git")
        .args(["add", "README"])
        .current_dir(root)
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "c"])
        .current_dir(root)
        .status()
        .expect("commit");

    let strace_out = tmp.path().join("strace.txt");
    let status = Command::new("strace")
        .args([
            "-f",
            "-e",
            "openat,open",
            "-o",
            strace_out.to_str().expect("utf8 path"),
            grit,
            "log",
            "--limit",
            "1",
        ])
        .current_dir(root)
        .status()
        .expect("strace grit log");
    assert!(status.success(), "grit log failed under strace");

    let log = std::fs::read_to_string(&strace_out).expect("read strace log");
    let git_dir = root.join(".git");
    let config_needle = format!("{}/config", git_dir.display());
    let commondir_needle = "commondir";
    let config_opens = count_path_opens(&log, &config_needle);
    let commondir_opens = count_path_opens(&log, commondir_needle);
    assert!(
        config_opens <= 2,
        "expected at most 2 opens of .git/config, got {config_opens}\n{log}"
    );
    assert!(
        commondir_opens <= 1,
        "expected at most 1 open of commondir, got {commondir_opens}\n{log}"
    );
    let _ = Path::new(grit);
}
