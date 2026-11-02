//! `grit clone <missing-local-path>` must fail before creating the destination (issue #2480).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .output()
        .expect("run grit")
}

#[test]
fn clone_typo_local_path_exits_128_without_creating_dest() {
    let scratch = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    let cwd = scratch.path();
    let dest = cwd.join("typo-repo");

    let out = grit(cwd, home.path(), &["clone", "typo-repo"]);
    assert_eq!(
        out.status.code(),
        Some(128),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not find repository"),
        "stderr: {stderr}"
    );
    assert!(!dest.exists(), "must not leave a bogus clone directory");

    let json_out = grit(cwd, home.path(), &["--json", "clone", "typo-repo"]);
    assert_eq!(json_out.status.code(), Some(128));
    let v: Value = serde_json::from_slice(&json_out.stdout).expect("json error");
    assert_eq!(v["kind"].as_str(), Some("remote_unreadable"));
    assert_ne!(v["no_checkout_reason"].as_str(), Some("empty_remote"));
}
