//! `grit add -N` / `--intent-to-add` and `-f` / `--force`.

use std::fs;
use std::process::Command;

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str], null: &str) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_CONFIG_SYSTEM", null)
        .output()
        .expect("run grit")
}

#[test]
fn add_intent_to_add_json_lists_paths() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();
    let root = dir.path();

    assert!(run_grit(grit, root, &["init"], &null).status.success());
    fs::write(root.join("n.txt"), b"new\n").expect("write");

    let out = run_grit(grit, root, &["add", "-N", "n.txt", "--json"], &null);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["intent_to_add"], serde_json::json!(["n.txt"]));
    assert_eq!(v["count"].as_u64(), Some(1));
}

#[test]
fn add_force_stages_ignored_file() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();
    let root = dir.path();

    assert!(run_grit(grit, root, &["init"], &null).status.success());
    fs::write(root.join(".gitignore"), "*.log\n").expect("gitignore");
    fs::write(root.join("trace.log"), b"x\n").expect("write");

    let denied = run_grit(grit, root, &["add", "trace.log"], &null);
    assert!(!denied.status.success());
    let err = String::from_utf8_lossy(&denied.stderr);
    assert!(
        err.contains("--force"),
        "error should mention --force: {err}"
    );

    let out = run_grit(grit, root, &["add", "-f", "trace.log", "--json"], &null);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["added"], serde_json::json!(["trace.log"]));
}
