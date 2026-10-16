//! `grit worktree lock` / `unlock` compatibility with system `git`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "grit-worktree-cli-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("tempdir");
    p
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn grit")
}

#[test]
fn worktree_lock_git_sees_locked() {
    let dir = unique_tmp("lock");
    git(&dir, &["init", "-q", "-b", "main", "."]);
    std::fs::write(dir.join("f"), b"1\n").unwrap();
    git(&dir, &["add", "f"]);
    git(&dir, &["commit", "-qm", "base"]);

    let wt = dir.join("linked");
    let out = grit(
        &dir,
        &["worktree", "add", wt.to_str().unwrap(), "-b", "side"],
    );
    assert!(
        out.status.success(),
        "add: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = grit(
        &dir,
        &["worktree", "lock", wt.to_str().unwrap(), "--reason", "hold"],
    );
    assert!(
        out.status.success(),
        "lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("locked") && porcelain.contains("hold"),
        "git must see grit lock:\n{porcelain}"
    );

    git(&dir, &["worktree", "unlock", wt.to_str().unwrap()]);
    git(
        &dir,
        &[
            "worktree",
            "lock",
            wt.to_str().unwrap(),
            "--reason",
            "from-git",
        ],
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain.contains("from-git"),
        "git lock must stick:\n{porcelain}"
    );
    let out = grit(&dir, &["worktree", "unlock", wt.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "unlock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let porcelain = git(&dir, &["worktree", "list", "--porcelain"]);
    assert!(
        !porcelain.contains("locked"),
        "git must see grit unlock:\n{porcelain}"
    );
}
