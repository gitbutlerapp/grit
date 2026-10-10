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
