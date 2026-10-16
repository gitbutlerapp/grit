//! Pathless `grit add` / `grit commit` after `git add -N` (intent-to-add).

use std::fs;
use std::process::Command;

use grit_lib::diff::empty_blob_oid;
use grit_lib::index::Index;

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

fn run_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run git")
}

#[test]
fn pathless_add_and_commit_after_git_add_n() {
    if !Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        return;
    }

    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();
    let root = dir.path();

    assert!(run_grit(grit, root, &["init"], &null).status.success());
    assert!(
        run_grit(grit, root, &["config", "user.name", "Test"], &null)
            .status
            .success()
    );
    assert!(run_grit(
        grit,
        root,
        &["config", "user.email", "t@example.com"],
        &null
    )
    .status
    .success());
    fs::write(root.join("f.txt"), b"hello\n").expect("write");
    assert!(run_git(root, &["add", "-N", "f.txt"]).status.success());

    let index_before = Index::load(&root.join(".git/index")).expect("index");
    let empty = empty_blob_oid();
    let ie = index_before.get(b"f.txt", 0).expect("ita row");
    assert!(ie.intent_to_add());
    assert_eq!(ie.oid, empty);

    let add_json = run_grit(grit, root, &["add", "--json"], &null);
    assert!(
        add_json.status.success(),
        "grit add failed: {}",
        String::from_utf8_lossy(&add_json.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&add_json.stdout).expect("add --json parses");
    assert_eq!(
        v["staged"].as_u64(),
        Some(1),
        "pathless add must report one staged change"
    );

    let index_after = Index::load(&root.join(".git/index")).expect("index after add");
    let ie = index_after.get(b"f.txt", 0).expect("staged f.txt");
    assert!(!ie.intent_to_add());
    assert_ne!(ie.oid, empty);

    let commit_out = run_grit(grit, root, &["commit", "-m", "promote ita"], &null);
    assert!(
        commit_out.status.success(),
        "grit commit failed: {}",
        String::from_utf8_lossy(&commit_out.stderr)
    );
}
