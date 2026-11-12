//! `grit stash push --hunks` partial stashes.

use std::fs;
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
        .env("GIT_AUTHOR_NAME", "A U Thor")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "C O Mitter")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .output()
        .expect("run grit")
}

fn two_hunk_file() -> (String, String) {
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
fn stash_push_hunks_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    let (base, edited) = two_hunk_file();
    fs::write(root.join("f.txt"), base).expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), edited).expect("write");

    let diff = run_grit(grit, root, &["diff", "--unstaged", "--json"]);
    assert!(diff.status.success());
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("hunk id");

    let stash = run_grit(
        grit,
        root,
        &["stash", "push", "--hunks", id, "-m", "partial", "--json"],
    );
    assert!(
        stash.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stash.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&stash.stdout).expect("stash json");
    assert!(out["oid"].as_str().is_some());
    assert!(
        out["message"]
            .as_str()
            .is_some_and(|m| m.contains("partial")),
        "message: {:?}",
        out["message"]
    );
    let hunks = out["stashed_hunks"]
        .as_object()
        .expect("stashed_hunks object");
    assert!(
        hunks.get("f.txt").is_some_and(|v| {
            v.as_array()
                .is_some_and(|a| a.iter().any(|entry| entry.as_str() == Some(id)))
        }),
        "stashed_hunks: {:?}",
        out["stashed_hunks"]
    );
    let on_disk = fs::read_to_string(root.join("f.txt")).unwrap();
    assert!(on_disk.contains("LINE18"));
    assert!(!on_disk.contains("LINE5"));
}

#[test]
fn stash_push_lines_dry_run_json_reports_lines_and_keeps_worktree() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    let (base, edited) = two_hunk_file();
    fs::write(root.join("f.txt"), base).expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), &edited).expect("write");

    let preview = run_grit(
        grit,
        root,
        &["stash", "push", "--lines", "f.txt:6", "--dry-run", "--json"],
    );
    assert!(
        preview.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&preview.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&preview.stdout).expect("stash json");
    assert_eq!(out["dry_run"], serde_json::json!(true));
    assert_eq!(out["paths"], serde_json::json!(["f.txt"]));
    assert_eq!(out["stashed_lines"], serde_json::json!({ "f.txt": ["6"] }));
    assert!(out.get("stashed_hunks").is_none(), "{out}");
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), edited);

    let conflict = run_grit(
        grit,
        root,
        &["stash", "push", "--lines", "f.txt:6", "--include-untracked"],
    );
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("--include-untracked"),
        "stderr: {}",
        String::from_utf8_lossy(&conflict.stderr)
    );
    assert_eq!(fs::read_to_string(root.join("f.txt")).unwrap(), edited);
}
