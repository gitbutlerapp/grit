//! `grit reflog --json` exposes old/new oids, absolute date, and committer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn grit_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_grit")
        .map(std::path::PathBuf::from)
        .expect("CARGO_BIN_EXE_grit")
}

fn run_grit(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(grit_bin())
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    assert!(
        out.status.success(),
        "grit {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn reflog_json_fields_cli() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(Command::new(grit_bin())
        .current_dir(dir.path())
        .args(["init"])
        .status()
        .expect("init")
        .success());
    fs::write(dir.path().join("f.txt"), "x\n").expect("write");
    run_grit(dir.path(), &["add", "f.txt"]);
    run_grit(dir.path(), &["commit", "-m", "one"]);
    fs::write(dir.path().join("g.txt"), "y\n").expect("write");
    run_grit(dir.path(), &["add", "g.txt"]);
    run_grit(dir.path(), &["commit", "-m", "two"]);

    let stdout = run_grit(dir.path(), &["reflog", "HEAD", "--json"]);
    let v: Value = serde_json::from_str(stdout.trim()).expect("json");
    let entries = v["entries"].as_array().expect("entries array");
    assert!(!entries.is_empty());
    let latest = &entries[0];
    for key in [
        "oid",
        "old_oid",
        "new_oid",
        "relative_date",
        "date",
        "message",
    ] {
        assert!(latest.get(key).is_some(), "missing key {key} in {latest}");
    }
    assert_eq!(latest["oid"], latest["new_oid"]);
    let committer = latest.get("committer").expect("committer object");
    for key in ["name", "email", "date"] {
        assert!(committer.get(key).is_some(), "missing committer.{key}");
    }
    assert_eq!(committer["date"], latest["date"]);
    assert!(
        latest["date"].as_str().is_some_and(|d| d.contains('T')
            && (d.ends_with('Z') || d.contains('+') || d.rfind('-').is_some_and(|i| i > 10))),
        "date should be RFC 3339: {}",
        latest["date"]
    );
}
