//! Non-UTF-8 file names: status, diff, add, and clone (issue #1399).

#![cfg(unix)]

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

use grit_lib::repo_relpath;
use grit_test_support::git;

const BAD_NAME: &[u8] = b"bad\xffname.txt";

fn bad_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(OsStr::from_bytes(BAD_NAME))
}

fn grit_bin() -> std::path::PathBuf {
    std::env::var_os("GRIT_BIN")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/release/grit")
        })
}

fn path_is_bad_name(stored: &str) -> bool {
    repo_relpath::storage_bytes(stored) == BAD_NAME
}

#[test]
fn non_utf8_tracked_status_diff_add_and_clone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let r1 = tmp.path().join("r1");
    std::fs::create_dir(&r1).expect("mkdir");
    git(&r1, &["init", "-q", "-b", "main"]);
    git(&r1, &["config", "user.email", "t@example.com"]);
    git(&r1, &["config", "user.name", "Test"]);
    std::fs::write(bad_path(&r1), b"one\n").expect("write bad");
    std::fs::write(r1.join("ok.txt"), b"ok\n").expect("write ok");
    git(&r1, &["add", "-A"]);
    git(&r1, &["commit", "-qm", "base"]);

    let mut bad = std::fs::OpenOptions::new()
        .append(true)
        .open(bad_path(&r1))
        .expect("open bad");
    use std::io::Write;
    bad.write_all(b"two\n").expect("append");
    std::fs::remove_file(r1.join("ok.txt")).expect("rm ok");

    let grit = grit_bin();
    let st = Command::new(&grit)
        .current_dir(&r1)
        .args(["status", "--json"])
        .output()
        .expect("grit status");
    assert!(
        st.status.success(),
        "{}",
        String::from_utf8_lossy(&st.stderr)
    );
    let st_json: serde_json::Value = serde_json::from_slice(&st.stdout).expect("json");
    let unstaged: Vec<_> = st_json["unstaged"]
        .as_array()
        .expect("unstaged array")
        .iter()
        .filter_map(|e| e["path"].as_str())
        .collect();
    assert!(
        unstaged.iter().any(|p| path_is_bad_name(p)),
        "modified non-UTF-8 path must appear in status json: {unstaged:?}"
    );
    assert!(
        unstaged.iter().any(|p| *p == "ok.txt"),
        "deleted ok.txt must appear: {unstaged:?}"
    );

    let diff = Command::new(&grit)
        .current_dir(&r1)
        .args(["diff", "--json"])
        .output()
        .expect("grit diff");
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let files = diff_json["files"].as_array().expect("files");
    assert!(
        files
            .iter()
            .filter_map(|f| f["path"].as_str())
            .any(path_is_bad_name),
        "diff must include non-UTF-8 path: {files:?}"
    );

    let clone_dest = tmp.path().join("r1c");
    let clone_out = Command::new(&grit)
        .args(["clone", r1.to_str().unwrap(), clone_dest.to_str().unwrap()])
        .output()
        .expect("grit clone");
    assert!(
        clone_out.status.success(),
        "{}",
        String::from_utf8_lossy(&clone_out.stderr)
    );
    assert!(
        bad_path(&clone_dest).exists(),
        "clone must create file with original byte name"
    );
    let git_st = Command::new("git")
        .current_dir(&clone_dest)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["status", "--short"])
        .output()
        .expect("git status clone");
    assert!(
        git_st.status.success(),
        "{}",
        String::from_utf8_lossy(&git_st.stderr)
    );
    assert!(
        git_st.stdout.is_empty(),
        "clone worktree must be clean for git: {}",
        String::from_utf8_lossy(&git_st.stdout)
    );

    let r1b = tmp.path().join("r1b");
    std::fs::create_dir(&r1b).expect("mkdir r1b");
    git(&r1b, &["init", "-q", "-b", "main"]);
    std::fs::write(bad_path(&r1b), b"x\n").expect("bad");
    std::fs::write(r1b.join("good.txt"), b"y\n").expect("good");
    let add_out = Command::new(&grit)
        .current_dir(&r1b)
        .arg("add")
        .output()
        .expect("grit add");
    assert!(
        add_out.status.success(),
        "grit add failed: {}",
        String::from_utf8_lossy(&add_out.stderr)
    );
    let git_st = Command::new("git")
        .current_dir(&r1b)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["status", "--short"])
        .output()
        .expect("git status after add");
    assert!(git_st.status.success());
    let porcelain = String::from_utf8_lossy(&git_st.stdout);
    assert!(
        porcelain.contains("good.txt"),
        "good.txt should be staged: {porcelain}"
    );
    assert!(
        porcelain.lines().count() >= 2,
        "both paths should be staged: {porcelain}"
    );
}
