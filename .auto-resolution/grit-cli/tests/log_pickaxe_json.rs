//! `grit log` pickaxe JSON vs git.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn git_log(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C", dir.to_str().expect("utf8"), "log", "--format=%H"];
    argv.extend(args);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn log_pickaxe_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("file"), "").expect("file");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("init");
    Command::new("git")
        .args(["add", "file"])
        .current_dir(dir.path())
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "initial"])
        .current_dir(dir.path())
        .env("GIT_AUTHOR_DATE", "@1000 +0000")
        .env("GIT_COMMITTER_DATE", "@1000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("commit");
    std::fs::write(dir.path().join("file"), "needle\n").expect("write");
    Command::new("git")
        .args(["add", "file"])
        .current_dir(dir.path())
        .status()
        .expect("add");
    Command::new("git")
        .args(["commit", "-qm", "second"])
        .current_dir(dir.path())
        .env("GIT_AUTHOR_DATE", "@2000 +0000")
        .env("GIT_COMMITTER_DATE", "@2000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("commit2");
    let git_oids = git_log(dir.path(), &["-S", "needle", "-n", "5"]);
    let json = grit_json(dir.path(), &["log", "-S", "needle", "-n", "5"]);
    let grit_oids: Vec<String> = json["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(git_oids, grit_oids);
}
