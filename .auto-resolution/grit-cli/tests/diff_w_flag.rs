//! `grit diff -w` / `-b` / `--ignore-blank-lines` human and JSON output.

use std::process::Command;

use grit_test_support::git;
use serde_json::Value;

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn diff_w_flag_json_and_human() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("ws.txt"), "hello world\n").expect("write");
    git(root, &["add", "ws.txt"]);
    git(root, &["commit", "-qm", "base"]);
    std::fs::write(root.join("ws.txt"), "helloworld\n").expect("rewrite");

    let (code, human, stderr) = grit(root, &["diff", "-w"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(
        human.trim().is_empty() || human.trim() == "No changes.",
        "whitespace-only change with -w should show no file hunks: {human:?}"
    );

    let (code, json, stderr) = grit(root, &["diff", "-w", "--json"]);
    assert_eq!(code, 0, "stderr={stderr}");
    let v: Value = serde_json::from_str(&json).expect("json");
    let files = v
        .get("files")
        .and_then(Value::as_array)
        .expect("files array");
    assert!(
        files.is_empty(),
        "whitespace-only change with -w should omit file from JSON: {json}"
    );
}
