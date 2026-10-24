//! Parallel `grit add` processes on one repository (issue #1792).
//!
//! Each process either stages its path or fails with an `index.lock` error the caller can
//! retry; a run that exits 0 must never be overwritten by a racing process.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t.io")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t.io")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit_add(dir: &Path, path: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(["add", path])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit add")
}

#[test]
fn parallel_add_processes_never_lose_a_successful_add() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("base"), "x\n").expect("write base");
    git(dir, &["add", "base"]);
    git(dir, &["commit", "-q", "-m", "init"]);

    let writers = 30;
    for i in 1..=writers {
        fs::write(dir.join(format!("n{i}.txt")), format!("{i}\n")).expect("write file");
    }
    let runs: Vec<(String, Output)> = thread::scope(|scope| {
        let handles: Vec<_> = (1..=writers)
            .map(|i| {
                scope.spawn(move || {
                    let path = format!("n{i}.txt");
                    let out = grit_add(dir, &path);
                    (path, out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("add thread"))
            .collect()
    });

    let staged: Vec<String> = git(dir, &["ls-files"]).lines().map(str::to_owned).collect();
    let mut succeeded = 0;
    for (path, out) in &runs {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            succeeded += 1;
            assert!(
                staged.contains(path),
                "`grit add {path}` exited 0 but the path is not in the index"
            );
        } else {
            assert!(
                stderr.contains("index.lock"),
                "`grit add {path}` failed for a reason other than lock contention: {stderr}"
            );
            assert!(!staged.contains(path), "{path} staged despite failing");
        }
    }
    assert!(succeeded > 0);
    assert_eq!(staged.len(), 1 + succeeded, "index: {staged:?}");
    git(dir, &["fsck", "--strict", "--no-progress"]);
}
