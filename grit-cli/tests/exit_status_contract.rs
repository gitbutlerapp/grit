//! Exit-code documentation contract and CLI `--json` `kind` values.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn grit(dir: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("grit");
    let code = out.status.code().unwrap_or(-1);
    (code, String::from_utf8_lossy(&out.stdout).into_owned())
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let (code, _) = grit(dir.path(), &["init"]);
    assert_eq!(code, 0);
    dir
}

#[test]
fn exit_codes_page_matches_enum() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../content/docs/exit-codes.md");
    let text = fs::read_to_string(&path).expect("read exit-codes.md");
    for slug in [
        "success",
        "failure",
        "usage",
        "remote_unreadable",
        "missing_prerequisite",
        "refused_not_ff",
        "invalid_commit_filter",
        "custom",
        "conflict",
        "hash_collision",
        "would_overwrite",
        "lock_held",
        "invalid_pattern",
        "incompatible_flags",
        "branch_already_exists",
        "branch_not_found",
        "branch_invalid_name",
        "branch_not_fully_merged",
        "branch_checked_out",
        "branch_no_commits",
        "branch_invalid_start_point",
        "branch_upstream_not_tracking",
        "branch_no_upstream",
        "branch_error",
    ] {
        assert!(
            text.contains(&format!("`{slug}`")),
            "exit-codes.md must document slug `{slug}`"
        );
    }
}

#[test]
fn exit_status_for_every_error_kind_cli() {
    let root = init_repo();
    let cases: &[(&str, &[&str], &str, i32)] = &[
        ("usage", &["branch", "--delete", "--json"], "usage", 2),
        (
            "incompatible_flags",
            &["log", "--graph", "--json", "-n", "1"],
            "incompatible_flags",
            1,
        ),
        (
            "invalid_pattern",
            &["log", "--author", "[", "--json", "-n", "1"],
            "invalid_pattern",
            129,
        ),
        (
            "branch_not_found",
            &["branch", "-d", "missing", "--json"],
            "branch_not_found",
            1,
        ),
        (
            "branch_invalid_name",
            &["branch", "bad..name", "--json"],
            "branch_invalid_name",
            1,
        ),
    ];

    for (name, args, kind, code) in cases {
        let (got_code, stdout) = grit(root.path(), args);
        assert_eq!(got_code, *code, "{name}");
        let v: Value = serde_json::from_str(stdout.trim()).expect(name);
        assert_eq!(v["kind"].as_str(), Some(*kind), "{name}");
    }

    fs::write(root.path().join("a.txt"), "a").expect("write");
    let (code, _) = grit(root.path(), &["commit", "-m", "seed", "--json"]);
    assert_eq!(code, 0);
    let _ = grit(root.path(), &["branch", "dup"]);
    let (code, stdout) = grit(root.path(), &["branch", "dup", "--json"]);
    assert_ne!(code, 0);
    let v: Value = serde_json::from_str(stdout.trim()).expect("branch_already_exists");
    assert_eq!(v["kind"].as_str(), Some("branch_already_exists"));
}

#[test]
fn held_index_lock_reports_lock_held_kind_with_path() {
    let root = init_repo();
    fs::write(root.path().join("n"), "n\n").expect("write");
    let lock = root.path().join(".git/index.lock");
    fs::write(&lock, b"").expect("hold index.lock");

    let (code, stdout) = grit(root.path(), &["--json", "add", "n"]);
    assert_eq!(code, 1, "{stdout}");
    let v: Value = serde_json::from_str(stdout.trim()).expect("json error");
    assert_eq!(v["kind"].as_str(), Some("lock_held"), "{v}");
    let path = v["lock"]["path"].as_str().expect("lock.path");
    assert_eq!(
        Path::new(path).canonicalize().expect("lock path exists"),
        lock.canonicalize().expect("lock")
    );
    assert!(v["lock"].get("holder").is_none(), "no pid sidecar: {v}");
    let error = v["error"].as_str().expect("error text");
    assert!(error.contains(path), "{error}");
    assert!(!error.contains("entity already exists"), "{error}");

    let pid_file = root.path().join(".git/index~pid.lock");
    fs::write(&pid_file, "pid 999999999\n").expect("stale pid sidecar");
    let (code, stdout) = grit(root.path(), &["--json", "add", "n"]);
    assert_eq!(code, 1, "{stdout}");
    let v: Value = serde_json::from_str(stdout.trim()).expect("json error");
    assert_eq!(v["kind"].as_str(), Some("lock_held"), "{v}");
    assert_eq!(
        v["lock"]["holder"]["pid"].as_u64(),
        Some(999_999_999),
        "{v}"
    );
    assert_eq!(v["lock"]["holder"]["running"].as_bool(), Some(false), "{v}");

    let human = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root.path())
        .args(["add", "n"])
        .output()
        .expect("grit");
    assert_eq!(human.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(stderr.contains("index.lock"), "{stderr}");
    assert!(stderr.contains("stale lock"), "{stderr}");
    assert!(lock.is_file(), "grit must not remove a foreign lock");

    fs::remove_file(&lock).expect("release lock");
    fs::remove_file(&pid_file).expect("remove pid sidecar");
    let (code, _) = grit(root.path(), &["add", "n"]);
    assert_eq!(code, 0, "add succeeds once the lock is gone");
}
