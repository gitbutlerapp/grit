//! Modern CLI: NFD worktree paths stage as NFC index entries when precompose is enabled.

use std::fs;
use std::process::Command;

use grit_lib::index::Index;
use serde_json::Value;

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn grit_env(null: &str) -> [(&str, &str); 3] {
    [
        ("GIT_TEST_UTF8_NFD_TO_NFC", "1"),
        ("GIT_CONFIG_GLOBAL", null),
        ("GIT_CONFIG_SYSTEM", null),
    ]
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str], null: &str) -> std::process::Output {
    let mut cmd = Command::new(grit);
    cmd.current_dir(dir).args(args);
    for (k, v) in grit_env(null) {
        cmd.env(k, v);
    }
    cmd.output().expect("run grit")
}

#[test]
fn grit_init_add_nfd_untracked_stores_nfc_in_index() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let null = null_config();

    let init = run_grit(grit, dir.path(), &["init"], &null);
    assert!(
        init.status.success(),
        "grit init failed: stderr={} stdout={}",
        String::from_utf8_lossy(&init.stderr),
        String::from_utf8_lossy(&init.stdout)
    );

    let nfd = format!("cafe\u{0301}.txt");
    fs::write(dir.path().join(&nfd), b"x\n").expect("write nfd file");

    let add = run_grit(grit, dir.path(), &["add"], &null);
    assert!(
        add.status.success(),
        "grit add failed: stderr={} stdout={}",
        String::from_utf8_lossy(&add.stderr),
        String::from_utf8_lossy(&add.stdout)
    );

    let index = Index::load(&dir.path().join(".git/index")).expect("load index");
    assert_eq!(index.entries.len(), 1, "expected one staged file");
    let path = &index.entries[0].path;
    assert_eq!(
        path.as_slice(),
        b"caf\xc3\xa9.txt",
        "index must store NFC UTF-8 spelling (issue #910)"
    );

    let status = run_grit(grit, dir.path(), &["status", "--json"], &null);
    assert!(
        status.status.success(),
        "grit status --json failed: stderr={} stdout={}",
        String::from_utf8_lossy(&status.stderr),
        String::from_utf8_lossy(&status.stdout)
    );
    let v: Value = serde_json::from_slice(&status.stdout).expect("parse status json");
    let unstaged = v["unstaged"].as_array().expect("unstaged array");
    assert!(
        unstaged.is_empty(),
        "expected no false unstaged deletions after add, got: {v}"
    );
    let staged = v["staged"].as_array().expect("staged array");
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["path"], "café.txt");

    let commit = run_grit(grit, dir.path(), &["commit", "-m", "one"], &null);
    assert!(
        commit.status.success(),
        "grit commit failed: stderr={} stdout={}",
        String::from_utf8_lossy(&commit.stderr),
        String::from_utf8_lossy(&commit.stdout)
    );

    use grit_lib::objects::{parse_commit, parse_tree};
    use grit_lib::repo::Repository;
    use grit_lib::state::resolve_head;

    let repo = Repository::discover(Some(dir.path())).expect("discover repo");
    let head_state = resolve_head(&repo.git_dir).expect("resolve head");
    let head = head_state.oid().expect("HEAD after commit").clone();
    let commit_obj = repo.odb.read(&head).expect("read commit");
    let commit_data = parse_commit(&commit_obj.data).expect("parse commit");
    let tree_obj = repo.odb.read(&commit_data.tree).expect("read tree");
    let tree_entries = parse_tree(&tree_obj.data).expect("parse tree");
    assert_eq!(tree_entries.len(), 1);
    assert_eq!(
        tree_entries[0].name.as_slice(),
        b"caf\xc3\xa9.txt",
        "commit tree must store NFC UTF-8 spelling"
    );

    let after = run_grit(grit, dir.path(), &["status", "--json"], &null);
    assert!(after.status.success());
    let after_v: Value = serde_json::from_slice(&after.stdout).expect("parse status json");
    assert_eq!(
        after_v["clean"],
        Value::Bool(true),
        "expected clean status after commit: {after_v}"
    );
}
