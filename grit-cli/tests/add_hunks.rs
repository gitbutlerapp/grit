//! `grit add --hunks` vs partial index updates.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit")
}

fn validate_add_v1_schema(instance: &serde_json::Value) {
    let schema_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../content/schemas/add.v1.json");
    let schema_text = fs::read_to_string(&schema_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", schema_path.display()));
    let schema: serde_json::Value = serde_json::from_str(&schema_text).expect("parse add.v1.json");
    let compiled =
        jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("compile add schema: {e}"));
    assert!(
        compiled.is_valid(instance),
        "add --json output failed schema validation: {:?}",
        compiled
            .iter_errors(instance)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
}

fn spaced_two_hunk_file() -> (String, String) {
    let mut base = String::new();
    for i in 0..24 {
        base.push_str(&format!("line{i}\n"));
    }
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "LINE5\n", 1);
    edited = edited.replacen("line18\n", "LINE18\n", 1);
    (base, edited)
}

#[test]
fn add_hunks_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    let (base, edited) = spaced_two_hunk_file();
    fs::write(root.join("f.txt"), base).expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), edited).expect("write");

    let diff = run_grit(grit, root, &["diff", "--unstaged", "--json"]);
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("hunk id");

    let add = run_grit(grit, root, &["add", "--hunks", id, "--json", "--", "f.txt"]);
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&add.stdout).expect("add json");
    assert_eq!(out["staged_hunks"]["f.txt"], serde_json::json!([id]));
    assert_eq!(out["modified"], serde_json::json!(["f.txt"]));
    validate_add_v1_schema(&out);
}

#[test]
fn add_plain_json_matches_schema() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt", "--json"])
        .status
        .success());
    let add = run_grit(grit, root, &["add", "--json"]);
    assert!(add.status.success());
    let out: serde_json::Value = serde_json::from_slice(&add.stdout).expect("add json");
    assert_eq!(out["staged_hunks"], serde_json::json!({}));
    assert_eq!(out["staged_lines"], serde_json::json!({}));
    validate_add_v1_schema(&out);
}

#[test]
fn add_hunks_unknown_id_exit_code() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), b"two\n").expect("write");

    let add = run_grit(grit, root, &["add", "--hunks", "000000000001", "--json"]);
    assert_eq!(add.status.code(), Some(2));
}

#[test]
fn add_hunks_unified_zero_context() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    let (base, edited) = spaced_two_hunk_file();
    fs::write(root.join("f.txt"), base).expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), edited).expect("write");

    let diff = run_grit(
        grit,
        root,
        &["diff", "--unstaged", "-U0", "--json", "--", "f.txt"],
    );
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("hunk id");

    let add = run_grit(
        grit,
        root,
        &["add", "--hunks", id, "-U0", "--json", "--", "f.txt"],
    );
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
}

#[test]
fn add_hunks_corrupt_index_not_unknown_id() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), b"two\n").expect("write");

    let diff = run_grit(grit, root, &["diff", "--unstaged", "--json"]);
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("id");

    fs::write(root.join(".git/index"), b"x").expect("truncate index");
    let add = run_grit(grit, root, &["add", "--hunks", id, "--json"]);
    assert_eq!(add.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&add.stderr);
    assert!(
        !stderr.contains("unknown hunk id"),
        "expected repository/index error, got: {stderr}"
    );
}
