//! Concurrent `grit commit -m MSG -- <path>` processes in one repository (issue #1791).
//!
//! Like Git, the index lock serializes the processes: each one either lands a commit carrying its
//! own message and only its own path, or fails with `index_locked`. The real index never ends up
//! stale and the history passes `git fsck`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const FILES: usize = 24;
const ROUNDS: usize = 4;

fn configure(cmd: &mut Command, dir: &Path) {
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Race")
        .env("GIT_AUTHOR_EMAIL", "race@example.com")
        .env("GIT_COMMITTER_NAME", "Race")
        .env("GIT_COMMITTER_EMAIL", "race@example.com");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    configure(&mut cmd, dir);
    let out = cmd.args(args).output().expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn spawn_commit(dir: &Path, i: usize) -> Child {
    let mut cmd = Command::new(GRIT);
    configure(&mut cmd, dir);
    cmd.args([
        "commit",
        "--json",
        "-m",
        &format!("msg-for-f{i}"),
        "--",
        &format!("f{i}.txt"),
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn grit")
}

#[test]
fn concurrent_path_commits_never_record_another_process_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..FILES {
        fs::write(dir.join(format!("f{i}.txt")), format!("{i}\n")).expect("seed");
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "init"]);

    let mut failures: BTreeMap<String, usize> = BTreeMap::new();
    let mut landed = 0usize;
    for round in 0..ROUNDS {
        let base = git(dir, &["rev-parse", "HEAD"]).trim().to_owned();
        for i in 0..FILES {
            let path = dir.join(format!("f{i}.txt"));
            let mut body = fs::read_to_string(&path).expect("read");
            body.push_str(&format!("r{round}\n"));
            fs::write(&path, body).expect("edit");
        }

        let children: Vec<Child> = (0..FILES).map(|i| spawn_commit(dir, i)).collect();
        for child in children {
            let out = child.wait_with_output().expect("wait grit");
            if out.status.success() {
                continue;
            }
            let err: Value = serde_json::from_slice(&out.stderr)
                .or_else(|_| serde_json::from_slice(&out.stdout))
                .unwrap_or_else(|_| {
                    panic!(
                        "unstructured failure: {}",
                        String::from_utf8_lossy(&out.stderr)
                    )
                });
            let kind = err["kind"].as_str().unwrap_or_default().to_owned();
            assert_eq!(kind, "index_locked", "unexpected failure: {err}");
            *failures.entry(kind).or_default() += 1;
        }

        let log = git(dir, &["log", "--format=%H %s", &format!("{base}..HEAD")]);
        for line in log.lines() {
            let (oid, subject) = line.split_once(' ').expect("log line");
            let files = git(dir, &["show", "--name-only", "--format=", oid]);
            let files: Vec<&str> = files.split_whitespace().collect();
            assert_eq!(files.len(), 1, "{oid} touches {files:?}");
            let expected = format!("msg-for-{}", files[0].trim_end_matches(".txt"));
            assert_eq!(subject, expected, "{oid} touches {files:?}");
            landed += 1;
        }

        assert_eq!(
            git(dir, &["diff", "--cached", "--name-only"]),
            "",
            "index is stale after round {round}"
        );
        git(dir, &["fsck", "--strict", "--no-progress"]);
        git(dir, &["reset", "-q", "--hard"]);
    }

    assert!(
        landed >= ROUNDS,
        "too few commits landed; failures: {failures:?}"
    );
}
