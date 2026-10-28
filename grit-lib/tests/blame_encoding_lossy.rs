//! Non-UTF-8 blame lines expose lossy decoding and raw bytes (issue #2260).
#![cfg(feature = "full-tests")]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::blame::{blame_file, BlameFinalSource, BlameOptions};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;

const LATIN1_FILE: &[u8] = &[
    0x63, 0x61, 0x66, 0xe9, 0x0a, 0x6e, 0x61, 0xef, 0x76, 0x65, 0x0a,
];

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "D")
        .env("GIT_AUTHOR_EMAIL", "d@e.com")
        .env("GIT_COMMITTER_NAME", "D")
        .env("GIT_COMMITTER_EMAIL", "d@e.com")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_latin1_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("l1.txt"), LATIN1_FILE).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "latin1"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    (tmp, repo)
}

fn assert_latin1_lines(result: &grit_lib::blame::BlameResult) {
    assert_eq!(result.lines.len(), 2);
    assert_eq!(result.lines[0].content, "caf\u{fffd}");
    assert!(result.lines[0].encoding_lossy);
    assert_eq!(
        result.lines[0].content_bytes.as_deref(),
        Some(b"caf\xe9".as_ref())
    );
    assert_eq!(result.lines[1].content, "na\u{fffd}ve");
    assert!(result.lines[1].encoding_lossy);
    assert_eq!(
        result.lines[1].content_bytes.as_deref(),
        Some(b"na\xefve".as_ref())
    );
}

#[test]
fn blame_file_revision_reports_encoding_lossy_and_bytes() {
    let (_tmp, repo) = init_latin1_repo();
    let result = blame_file(&repo, "l1.txt", &BlameOptions::default()).unwrap();
    assert_latin1_lines(&result);
}

#[test]
fn blame_file_worktree_reports_encoding_lossy_and_bytes() {
    let (tmp, repo) = init_latin1_repo();
    std::fs::write(tmp.path().join("l1.txt"), LATIN1_FILE).unwrap();
    let mut options = BlameOptions::default();
    options.final_source = BlameFinalSource::Worktree;
    let result = blame_file(&repo, "l1.txt", &options).unwrap();
    assert_latin1_lines(&result);
}

#[test]
fn blame_file_contents_reports_encoding_lossy_and_bytes() {
    let (_tmp, repo) = init_latin1_repo();
    let mut options = BlameOptions::default();
    options.final_source = BlameFinalSource::Contents(LATIN1_FILE.to_vec());
    let result = blame_file(&repo, "l1.txt", &options).unwrap();
    assert_latin1_lines(&result);
}

const MIXED_FILE: &[u8] = b"plain ascii\ncaf\xe9\n";

fn init_mixed_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("mixed.txt"), MIXED_FILE).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "mixed"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    (tmp, repo)
}

#[test]
fn mixed_encoding_flags_only_invalid_lines() {
    let (_tmp, repo) = init_mixed_repo();
    let result = blame_file(&repo, "mixed.txt", &BlameOptions::default()).unwrap();
    assert_eq!(result.lines.len(), 2);
    assert_eq!(result.lines[0].content, "plain ascii");
    assert!(!result.lines[0].encoding_lossy);
    assert!(result.lines[0].content_bytes.is_none());
    assert_eq!(result.lines[1].content, "caf\u{fffd}");
    assert!(result.lines[1].encoding_lossy);
    assert_eq!(
        result.lines[1].content_bytes.as_deref(),
        Some(b"caf\xe9".as_ref())
    );
}

#[test]
fn utf8_blame_stays_lossless() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("utf8.txt"), "hello\nworld\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "utf8"]);
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let result = blame_file(&repo, "utf8.txt", &BlameOptions::default()).unwrap();
    assert_eq!(result.lines.len(), 2);
    for line in &result.lines {
        assert!(!line.encoding_lossy);
        assert!(line.content_bytes.is_none());
    }
}

#[test]
fn non_utf8_line_diff_blames_child_commit_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("l1.txt"), LATIN1_FILE).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "parent"]);
    let parent_head = git_output(root, &["rev-parse", "HEAD"]);

    let child_bytes: &[u8] = &[
        0x63, 0x61, 0x66, 0xea, 0x0a, 0x6e, 0x61, 0xef, 0x76, 0x65, 0x0a,
    ];
    std::fs::write(root.join("l1.txt"), child_bytes).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "child"]);
    let child_head = git_output(root, &["rev-parse", "HEAD"]);

    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).unwrap();
    let parent_oid = ObjectId::from_hex(parent_head.trim()).unwrap();
    let child_oid = ObjectId::from_hex(child_head.trim()).unwrap();
    let mut options = BlameOptions::default();
    options.start = Some(child_oid);
    let result = blame_file(&repo, "l1.txt", &options).unwrap();
    assert_eq!(result.lines[0].commit, child_oid);
    assert_eq!(result.lines[1].commit, parent_oid);
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}
