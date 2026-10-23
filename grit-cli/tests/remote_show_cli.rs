//! `grit remote show` JSON and flags.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn grit_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn remote_show_json_fields_and_flags() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let upstream = tmp.path().join("upstream");
    let clone = tmp.path().join("clone");
    std::fs::create_dir_all(&upstream).expect("upstream");
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["commit", "--allow-empty", "-q", "-m", "init"]);
    git(&upstream, &["branch", "topic"]);
    git(
        tmp.path(),
        &["clone", "-q", upstream.to_str().unwrap(), "clone"],
    );

    let value = grit_json(&clone, &["remote", "show", "origin", "--json"]);
    assert_eq!(value["action"], "show");
    assert_eq!(value["name"], "origin");
    assert!(value["fetch_url"].is_string());
    assert!(value["push_url"].is_string());
    assert!(
        value["head_branch"].is_string() || value["head_branch"].is_null(),
        "head_branch: {:?}",
        value["head_branch"]
    );
    assert!(value["branches"].is_array());
    assert!(value["pull"].is_array());

    let no_query = grit_json(
        &clone,
        &["remote", "show", "origin", "--no-query", "--json"],
    );
    assert!(no_query["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .all(|b| b["state"] == "tracked"));

    let limited = grit_json(
        &clone,
        &[
            "remote",
            "show",
            "origin",
            "--no-query",
            "-n",
            "1",
            "--json",
        ],
    );
    assert_eq!(limited["branches"].as_array().map(Vec::len), Some(1));
}
