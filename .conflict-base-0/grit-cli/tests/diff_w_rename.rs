//! Rename metadata survives `grit diff -w`.

use std::process::Command;

use grit_test_support::git;

fn grit_json(dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn diff_w_staged_rename_shows_file_metadata() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    std::fs::write(root.join("old.txt"), "same\n").expect("write");
    git(root, &["add", "old.txt"]);
    git(root, &["commit", "-qm", "base"]);
    std::fs::rename(root.join("old.txt"), root.join("new.txt")).expect("rename");
    git(root, &["add", "old.txt", "new.txt"]);

    let v = grit_json(root, &["diff", "--staged", "-w", "--json"]);
    let files = v["files"].as_array().expect("files");
    assert_eq!(files.len(), 1, "{v}");
    assert_eq!(files[0]["path"], "new.txt");
    assert_eq!(files[0]["old_path"], "old.txt");
}
