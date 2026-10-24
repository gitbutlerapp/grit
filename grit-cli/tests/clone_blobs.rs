//! `grit clone --blobs` partial clone.

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

fn setup_filtering_bare(base: &std::path::Path) -> std::path::PathBuf {
    let wt = base.join("upstream");
    std::fs::create_dir_all(&wt).unwrap();
    git_in(&wt, &["init", "-q", "-b", "main", "."]);
    git_in(&wt, &["config", "user.email", "t@example.com"]);
    git_in(&wt, &["config", "user.name", "Test"]);
    std::fs::write(wt.join("small.txt"), b"tiny\n").unwrap();
    std::fs::write(wt.join("large.txt"), vec![b'x'; 4096]).unwrap();
    git_in(&wt, &["add", "."]);
    git_in(&wt, &["commit", "-q", "-m", "seed"]);
    git_in(&wt, &["branch", "feature"]);
    git_in(&wt, &["tag", "v1"]);
    let bare = base.join("remote.git");
    git_in(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    git_in(&bare, &["config", "uploadpack.allowFilter", "true"]);
    bare
}

#[test]
fn clone_blobs_none_json_fields() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_filtering_bare(base.path());
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = base.path().join("blobless");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--blobs",
            "none",
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
    assert_eq!(v["blobs"], "none");
    assert_eq!(v["partial"], true);
    assert_eq!(
        git_in_read_config(&dest, "remote.origin.partialclonefilter"),
        "blob:none"
    );
}

fn git_in_read_config(repo: &std::path::Path, key: &str) -> String {
    let out = Command::new("git")
        .args(["config", key])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git config");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn clone_blobs_limit_keeps_small_blobs() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_filtering_bare(base.path());
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = base.path().join("limited");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--blobs",
            "1k",
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
    assert_eq!(v["blobs"], 1024);
    assert_eq!(v["partial"], true);
    let small_oid = git_rev_parse(&dest, "HEAD:small.txt");
    assert!(git_cat_file_ok(&dest, &small_oid), "small blob present");
    assert_eq!(
        git_in_read_config(&dest, "remote.origin.partialclonefilter"),
        "blob:limit=1024"
    );
    let git_dest = base.path().join("git-limit");
    git_in(
        base.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:limit=1k",
            url.as_str(),
            git_dest.file_name().unwrap().to_str().unwrap(),
        ],
    );
    assert_eq!(
        count_pack_blobs(&dest),
        count_pack_blobs(&git_dest),
        "initial pack blob count should match git"
    );
}

fn count_pack_blobs(repo: &std::path::Path) -> usize {
    let pack_dir = repo.join(".git/objects/pack");
    let pack = std::fs::read_dir(&pack_dir)
        .expect("pack dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "pack"))
        .expect("pack file");
    let out = Command::new("git")
        .args(["verify-pack", "-v"])
        .arg(&pack)
        .output()
        .expect("verify-pack");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(" blob "))
        .count()
}

fn git_rev_parse(repo: &std::path::Path, spec: &str) -> String {
    let out = Command::new("git")
        .args(["rev-parse", spec])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("rev-parse");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_cat_file_ok(repo: &std::path::Path, oid: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e", oid])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn clone_blobs_invalid_size_exit_2() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_filtering_bare(base.path());
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = base.path().join("bad");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--blobs",
            "not-a-size",
            "--json",
            url.as_str(),
            dest.to_str().unwrap(),
        ])
        .output()
        .expect("grit clone");
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json error");
    assert!(v.get("error").is_some());
}

#[test]
fn clone_blobs_with_depth() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_filtering_bare(base.path());
    git_in(
        &bare.parent().unwrap().join("upstream"),
        &["commit", "-q", "--allow-empty", "-m", "c2"],
    );
    git_in(
        &bare.parent().unwrap().join("upstream"),
        &["push", bare.to_str().unwrap(), "main"],
    );
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = base.path().join("shallow-blobless");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--blobs",
            "none",
            "--depth",
            "1",
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
    assert_eq!(v["partial"], true);
    assert!(dest.join(".git/shallow").is_file());
}

#[test]
fn git_log_patch_works_in_grit_blobless_clone() {
    let base = tempfile::tempdir().expect("tempdir");
    let bare = setup_filtering_bare(base.path());
    let url = format!("file://{}", bare.canonicalize().unwrap().display());
    let dest = base.path().join("roundtrip");
    let out = Command::new(GRIT)
        .args([
            "clone",
            "--blobs",
            "none",
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
    git_in(&dest, &["fsck", "--no-dangling"]);
    let log = Command::new("git")
        .args(["log", "-1", "-p"])
        .current_dir(&dest)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git log -p");
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    let text = String::from_utf8_lossy(&log.stdout);
    assert!(text.contains("diff --git") || text.contains("small.txt"));
}
