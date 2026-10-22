//! `grit bundle` CLI compatibility and JSON output.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("grit-bundle-cli-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("tempdir");
    p
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit")
}

#[test]
fn bundle_create_accepted_by_git_verify_and_clone() {
    let dir = unique_tmp("create");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("README"), b"bundle test\n").unwrap();
    git(&dir, &["add", "README"]);
    git(&dir, &["commit", "-qm", "initial"]);

    let bundle = dir.join("out.bundle");
    let out = grit(
        &dir,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/main",
        ],
    );
    assert!(
        out.status.success(),
        "grit bundle create: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    git(&dir, &["bundle", "verify", bundle.to_str().unwrap()]);

    let clone_dir = dir.join("cloned");
    git(
        &dir,
        &[
            "clone",
            bundle.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ],
    );
    assert!(clone_dir.join(".git").is_dir());
}

#[test]
fn bundle_verify_and_list_json_schema() {
    let dir = unique_tmp("json");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f"), b"x\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "c"]);
    let bundle = dir.join("b.bundle");
    git(
        &dir,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/heads/main",
        ],
    );

    let verify = grit(
        &dir,
        &["bundle", "verify", bundle.to_str().unwrap(), "--json"],
    );
    assert!(verify.status.success());
    let v: Value = serde_json::from_slice(&verify.stdout).expect("verify json");
    assert_eq!(v["ok"], true);
    assert!(v["references"].is_array());
    assert!(v["hash_algorithm"].is_string());

    let list = grit(
        &dir,
        &["bundle", "list", bundle.to_str().unwrap(), "--json"],
    );
    assert!(list.status.success());
    let l: Value = serde_json::from_slice(&list.stdout).expect("list json");
    assert!(l["references"].is_array());
    assert!(l["path"].is_string());
}

#[test]
fn bundle_create_all_matches_git_list_heads() {
    let dir = unique_tmp("all");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("a"), b"a\n").unwrap();
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "one"]);
    git(&dir, &["tag", "-a", "rel-1", "-m", "r"]);
    git(&dir, &["tag", "lw"]);
    git(&dir, &["switch", "-qc", "feature/v1.2/x"]);
    std::fs::write(dir.join("b"), b"b\n").unwrap();
    git(&dir, &["add", "b"]);
    git(&dir, &["commit", "-qm", "two"]);
    git(&dir, &["switch", "-q", "main"]);

    let grit_bundle = dir.join("grit-all.bundle");
    let out = grit(
        &dir,
        &[
            "bundle",
            "create",
            grit_bundle.to_str().unwrap(),
            "--all",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "grit bundle create --all: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["refs"], 5);

    git(&dir, &["bundle", "verify", grit_bundle.to_str().unwrap()]);

    let git_bundle = dir.join("git-all.bundle");
    git(
        &dir,
        &["bundle", "create", git_bundle.to_str().unwrap(), "--all"],
    );
    let git_heads = Command::new("git")
        .current_dir(&dir)
        .args(["bundle", "list-heads", git_bundle.to_str().unwrap()])
        .output()
        .expect("git list-heads");
    let grit_heads = Command::new("git")
        .current_dir(&dir)
        .args(["bundle", "list-heads", grit_bundle.to_str().unwrap()])
        .output()
        .expect("grit list-heads");
    let mut git_lines: Vec<_> = String::from_utf8_lossy(&git_heads.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let mut grit_lines: Vec<_> = String::from_utf8_lossy(&grit_heads.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    git_lines.sort();
    grit_lines.sort();
    assert_eq!(git_lines, grit_lines);
}

#[test]
fn bundle_create_range_matches_git_oracle() {
    let dir = unique_tmp("range");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    git(&dir, &["commit", "--allow-empty", "-qm", "a"]);
    git(&dir, &["commit", "--allow-empty", "-qm", "b"]);

    let git_bundle = dir.join("git.bundle");
    git(
        &dir,
        &[
            "bundle",
            "create",
            git_bundle.to_str().unwrap(),
            "HEAD~1..HEAD",
        ],
    );

    let grit_bundle = dir.join("grit.bundle");
    let out = grit(
        &dir,
        &[
            "bundle",
            "create",
            grit_bundle.to_str().unwrap(),
            "HEAD~1..HEAD",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "grit bundle create range: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let git_heads = Command::new("git")
        .current_dir(&dir)
        .args(["bundle", "list-heads", git_bundle.to_str().unwrap()])
        .output()
        .expect("git list-heads");
    assert!(git_heads.status.success());
    let grit_heads = Command::new("git")
        .current_dir(&dir)
        .args(["bundle", "list-heads", grit_bundle.to_str().unwrap()])
        .output()
        .expect("grit list-heads");
    assert!(
        grit_heads.status.success(),
        "git list-heads grit bundle: {}",
        String::from_utf8_lossy(&grit_heads.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_heads.stdout).trim(),
        String::from_utf8_lossy(&grit_heads.stdout).trim()
    );

    let git_verify = Command::new("git")
        .current_dir(&dir)
        .args(["bundle", "verify", grit_bundle.to_str().unwrap()])
        .output()
        .expect("verify grit bundle");
    assert!(
        git_verify.status.success(),
        "git bundle verify grit output: {}",
        String::from_utf8_lossy(&git_verify.stderr)
    );
}

#[test]
fn bundle_verify_rejects_unknown_v3_capability() {
    let dir = unique_tmp("v3-cap");
    let tampered = dir.join("tampered.bundle");
    std::fs::write(
        &tampered,
        b"# v3 git bundle\n@object-format=sha1\n@unknown-grit-review=1\n\nPACK",
    )
    .expect("write tampered");

    let out = grit(
        &dir,
        &["bundle", "verify", tampered.to_str().unwrap(), "--json"],
    );
    assert!(!out.status.success());
}

#[test]
fn bundle_verify_missing_prerequisite_exit_code() {
    let upstream = unique_tmp("prereq-up");
    git(&upstream, &["init", "-q", "-b", "main", "."]);
    git(&upstream, &["commit", "--allow-empty", "-qm", "a"]);
    git(&upstream, &["commit", "--allow-empty", "-qm", "b"]);
    let bundle = upstream.join("partial.bundle");
    git(
        &upstream,
        &["bundle", "create", bundle.to_str().unwrap(), "HEAD~1..HEAD"],
    );

    let dest = unique_tmp("prereq-down");
    git(&dest, &["init", "-q", "-b", "main", "."]);
    git(&dest, &["commit", "--allow-empty", "-qm", "only"]);

    let out = grit(&dest, &["fetch", bundle.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(128),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn parse_bundle_list_heads(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (oid, name) = line.split_once(' ')?;
            Some((oid.to_string(), name.to_string()))
        })
        .collect()
}

#[test]
fn unbundle_then_git_fsck() {
    let src = unique_tmp("unbundle-src");
    git(&src, &["init", "-q", "-b", "main", "."]);
    std::fs::write(src.join("f"), b"payload\n").unwrap();
    git(&src, &["add", "f"]);
    git(&src, &["commit", "-qm", "c1"]);
    let bundle = src.join("transfer.bundle");
    git(
        &src,
        &["bundle", "create", bundle.to_str().unwrap(), "HEAD"],
    );

    let dest = unique_tmp("unbundle-dest");
    git(&dest, &["init", "-q", "--bare", "-b", "main", "."]);

    let out = grit(
        &dest,
        &["bundle", "unbundle", bundle.to_str().unwrap(), "--json"],
    );
    assert!(
        out.status.success(),
        "grit bundle unbundle: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("json");
    let grit_refs: Vec<(String, String)> = json["references"]
        .as_array()
        .expect("references")
        .iter()
        .map(|r| {
            (
                r["oid"].as_str().expect("oid").to_string(),
                r["name"].as_str().expect("name").to_string(),
            )
        })
        .collect();

    let heads = Command::new("git")
        .current_dir(&src)
        .args(["bundle", "list-heads", bundle.to_str().unwrap()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git list-heads");
    assert!(heads.status.success());
    let expected = parse_bundle_list_heads(&String::from_utf8_lossy(&heads.stdout));
    assert_eq!(
        grit_refs, expected,
        "imported heads must match bundle header"
    );

    git(&dest, &["fsck", "--strict", "--no-dangling"]);
}
