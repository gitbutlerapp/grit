//! `grit clone --depth` and `-b/--branch`.

use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn setup_bare(base: &std::path::Path) -> std::path::PathBuf {
    let wt = base.join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    git_in(&wt, &["init", "-q", "-b", "main", "."]);
    git_in(&wt, &["config", "user.email", "t@example.com"]);
    git_in(&wt, &["config", "user.name", "Test"]);
    for i in 1..=4 {
        std::fs::write(wt.join("f"), format!("{i}\n")).unwrap();
        git_in(&wt, &["add", "f"]);
        git_in(&wt, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git_in(&wt, &["branch", "feature", "HEAD~1"]);
    git_in(&wt, &["tag", "v1"]);
    let bare = base.join("remote.git");
    git_in(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    bare
}

#[test]
fn clone_depth_and_branch_json() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_bare(base.path());
    let url = format!("file://{}", bare.to_string_lossy());
    let dest = base.path().join("clone");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--depth",
            "2",
            "-b",
            "feature",
            "--json",
            url.as_str(),
            dest.to_str().unwrap(),
        ])
        .output()
        .expect("grit clone");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["branch"], "feature");
    assert_eq!(v["detached"], false);
    assert!(v.get("tag").is_none() || v["tag"].is_null());
    assert_eq!(v["depth"], 2);
}

#[test]
fn clone_tag_json_detached() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_bare(base.path());
    let url = format!("file://{}", bare.to_string_lossy());
    let dest = base.path().join("clone-tag");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "-b",
            "v1",
            "--json",
            url.as_str(),
            dest.to_str().unwrap(),
        ])
        .output()
        .expect("grit clone");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert!(v["branch"].is_null());
    assert_eq!(v["detached"], true);
    assert_eq!(v["tag"], "v1");
}

#[test]
fn clone_shallow_hidden_alias_still_works() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_bare(base.path());
    let url = format!("file://{}", bare.to_string_lossy());
    let dest = base.path().join("shallow");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--shallow",
            "--json",
            url.as_str(),
            dest.to_str().unwrap(),
        ])
        .output()
        .expect("grit clone");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["depth"], 1);
    assert!(dest.join(".git/shallow").is_file());
}

#[test]
fn clone_shallow_conflicts_with_depth() {
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--shallow",
            "--depth",
            "2",
            "file:///tmp/nope.git",
            "dest",
        ])
        .output()
        .expect("grit clone");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("cannot be used with") || err.contains("conflict"),
        "expected clap conflict message: {err}"
    );
}
