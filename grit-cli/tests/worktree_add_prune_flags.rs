//! CLI flags for `grit worktree add` and `prune --dry-run`.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn unique_tmp(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "grit-wt-flags-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("tempdir");
    p
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}");
}

fn grit_json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .args(["--json"])
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn worktree_add_json_includes_locked_and_checked_out() {
    let dir = unique_tmp("add-json");
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f"), b"x\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "c"]);

    let wt = dir.join("wt");
    let v = grit_json(
        &dir,
        &[
            "worktree",
            "add",
            wt.to_str().unwrap(),
            "-b",
            "side",
            "--lock",
            "--reason",
            "hold",
        ],
    );
    assert_eq!(v["action"], "add");
    assert_eq!(v["locked"], true);
    assert_eq!(v["checked_out"], true);

    let v2 = grit_json(
        &dir,
        &[
            "worktree",
            "add",
            dir.join("wt2").to_str().unwrap(),
            "-B",
            "other",
            "--no-checkout",
        ],
    );
    assert_eq!(v2["checked_out"], false);
}

#[test]
fn worktree_prune_dry_run_json() {
    let dir = unique_tmp("prune-json");
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("f"), b"x\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "c"]);

    let wt = dir.join("gone");
    grit_json(
        &dir,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "gone"],
    );
    std::fs::remove_dir_all(&wt).unwrap();

    let v = grit_json(&dir, &["worktree", "prune", "--dry-run"]);
    assert_eq!(v["action"], "prune");
    assert_eq!(v["dry_run"], true);
    assert!(v["pruned"].as_array().unwrap().len() >= 1);
    assert!(v["pruned"][0]["reason"].as_str().is_some());

    let list = grit_json(&dir, &["worktree", "list"]);
    assert_eq!(list["worktrees"].as_array().unwrap().len(), 2);
}
