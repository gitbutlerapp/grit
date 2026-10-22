//! `grit restore --ours` / `--theirs` CLI wiring.

use std::fs;
use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn git(repo: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn grit_json(repo: &Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = Command::new(GRIT);
    cmd.current_dir(repo).arg("--json").args(args);
    let out = cmd.output().expect("grit");
    assert!(
        out.status.success(),
        "grit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn grit(repo: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new(GRIT)
        .current_dir(repo)
        .args(args)
        .output()
        .expect("grit");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn init_conflict_repo(dir: &Path) {
    git(dir, &["init"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
    fs::write(dir.join("f.txt"), "line1\nline2\nline3\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-qm", "base"]);
    git(dir, &["branch", "side"]);
    git(dir, &["switch", "side"]);
    fs::write(dir.join("f.txt"), "line1\nside\nline3\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-qm", "side"]);
    git(dir, &["switch", "master"]);
    fs::write(dir.join("f.txt"), "line1\nmaster\nline3\n").unwrap();
    git(dir, &["add", "f.txt"]);
    git(dir, &["commit", "-qm", "master"]);
    git(dir, &["merge", "side"]);
}

fn remerge(dir: &Path) {
    let _ = git(dir, &["merge", "--abort"]);
    git(dir, &["switch", "master"]);
    git(dir, &["merge", "side"]);
}

#[test]
fn restore_ours_theirs_cli() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_conflict_repo(dir.path());

    git(dir.path(), &["restore", "--ours", "f.txt"]);
    let want_ours = fs::read(dir.path().join("f.txt")).unwrap();

    remerge(dir.path());
    let json = grit_json(dir.path(), &["restore", "--ours", "f.txt"]);
    assert_eq!(json["restored"], serde_json::json!(["f.txt"]));
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), want_ours);

    remerge(dir.path());
    git(dir.path(), &["restore", "--theirs", "f.txt"]);
    let want_theirs = fs::read(dir.path().join("f.txt")).unwrap();

    remerge(dir.path());
    let json = grit_json(dir.path(), &["restore", "--theirs", "f.txt"]);
    assert_eq!(json["restored"], serde_json::json!(["f.txt"]));
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), want_theirs);

    remerge(dir.path());
    let staged = grit(dir.path(), &["restore", "--staged", "--ours", "f.txt"]);
    assert_ne!(staged.status, Some(0));
}
